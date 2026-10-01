//! Renders debugger replies other than values.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

use uscope::{
    Backtrace, Breakpoint, BreakpointLocation, ByteOrder, CoreDumpInfo, CoreModuleState,
    ExitStatus, FunctionInfo, GlobalVariablePage, InvalidatedWatchpoint, LoadedModuleSnapshot,
    MemoryRead, MemoryReadCompletion, ModuleId, ModuleIdentity, ModuleImage, PointerWidth,
    RegisterRole, RegisterSnapshot, SourceContext, StateSnapshot, StepKind, StopReason,
    SymbolExtentProvenance, SymbolLocation, ThreadState, WatchScope, Watchpoint, WatchpointHit,
    WatchpointInvalidation,
};

use super::commands::{COMMANDS, CommandSpec};
use super::terminal::{Renderer, Role};
use super::value::{self, bound_output};

const HEX_DUMP_BYTES_PER_LINE: usize = 16;

/// Returns `count noun`, adding an `s` unless the count is one.
pub fn plural(count: u64, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

/// Joins the lines produced by `line`, or returns `empty` when there are none.
fn lines_or<T>(
    items: &[T],
    empty: &str,
    renderer: Renderer,
    line: impl Fn(&T) -> String,
) -> String {
    if items.is_empty() {
        return renderer.paint(Role::Metadata, empty).to_string();
    }
    items.iter().map(line).collect::<Vec<_>>().join("\n")
}

pub fn help(renderer: Renderer) -> String {
    let name_width = COMMANDS.iter().map(|command| command.name.len()).max();
    let alias_width = COMMANDS
        .iter()
        .map(|command| command.aliases.join(", ").len())
        .max();
    let (name_width, alias_width) = (name_width.unwrap_or(0), alias_width.unwrap_or(0));
    let mut output = "commands:".to_owned();
    for command in COMMANDS {
        let aliases = command.aliases.join(", ");
        let rendered_aliases = if aliases.is_empty() {
            String::new()
        } else {
            renderer.paint(Role::Alias, &aliases).to_string()
        };
        write!(
            output,
            "\n  {}{}  {rendered_aliases}{}  {}",
            renderer.paint(Role::Command, command.name),
            " ".repeat(name_width - command.name.len()),
            " ".repeat(alias_width - aliases.len()),
            command.summary
        )
        .expect("writing to a String cannot fail");
    }
    output.push_str("\n\nUse `help <command>` for aliases and usage.");
    output
}

pub fn command_help(command: &CommandSpec, renderer: Renderer) -> String {
    let mut output = format!("  {}", command.summary);
    if !command.aliases.is_empty() {
        write!(
            output,
            "\n  {}: {}",
            renderer.paint(Role::Muted, "aliases"),
            renderer.paint(Role::Alias, command.aliases.join(", "))
        )
        .expect("writing to a String cannot fail");
    }
    if command.takes_arguments() {
        write!(
            output,
            "\n  {}: {}",
            renderer.paint(Role::Muted, "usage"),
            command.usage
        )
        .expect("writing to a String cannot fail");
    }
    output
}

pub fn deleted(what: &str, renderer: Renderer) -> String {
    format!("{} {what}", renderer.paint(Role::Success, "deleted"))
}

fn breakpoint_location(location: BreakpointLocation, renderer: Renderer) -> String {
    match location {
        BreakpointLocation::Image(address) => {
            format!("image address {}", renderer.paint(Role::Metadata, address))
        }
        BreakpointLocation::Virtual(address) => {
            format!(
                "virtual address {}",
                renderer.paint(Role::Metadata, address)
            )
        }
    }
}

pub fn breakpoint(breakpoint: &Breakpoint, renderer: Renderer) -> String {
    let heading = format!(
        "{} {} set",
        renderer.paint(Role::Success, "breakpoint"),
        renderer.paint(Role::Metadata, breakpoint.id),
    );
    if let [resolved] = breakpoint.locations.as_ref() {
        return format!(
            "{heading} at {}",
            breakpoint_location(resolved.location, renderer)
        );
    }
    let mut output = format!("{heading} at {} locations", breakpoint.locations.len());
    for resolved in breakpoint.locations.iter() {
        write!(
            output,
            "\n  {}",
            breakpoint_location(resolved.location, renderer)
        )
        .expect("writing to a String cannot fail");
    }
    output
}

pub fn breakpoints(breakpoints: &[Breakpoint], renderer: Renderer) -> String {
    lines_or(breakpoints, "no breakpoints", renderer, |breakpoint| {
        let mut output = format!(
            "{}  {}  {}",
            renderer.paint(Role::Metadata, breakpoint.id),
            renderer.paint(Role::Name, &breakpoint.spec),
            plural(breakpoint.locations.len() as u64, "location")
        );
        for resolved in breakpoint.locations.iter() {
            write!(
                output,
                "\n  {}",
                breakpoint_location(resolved.location, renderer)
            )
            .expect("writing to a String cannot fail");
        }
        output
    })
}

/// Names a watchpoint by the expression that resolved it, or by its bytes.
fn watch_subject(watchpoint: &Watchpoint) -> String {
    watchpoint.expression.as_ref().map_or_else(
        || format!("{}:{}", watchpoint.address, watchpoint.byte_size),
        ToString::to_string,
    )
}

fn watch_scope_suffix(scope: &WatchScope) -> String {
    match scope {
        WatchScope::Location | WatchScope::Static { .. } => String::new(),
        WatchScope::ThreadLocal { thread } => format!(" (thread {thread}'s instance)"),
        WatchScope::Frame { thread, activation } => {
            format!(" (frame {activation} of thread {thread})")
        }
    }
}

pub fn watchpoint_set(watchpoint: &Watchpoint, renderer: Renderer) -> String {
    format!(
        "{} {} set on {}: {} at {} using {}{}",
        renderer.paint(Role::Success, "watchpoint"),
        renderer.paint(Role::Metadata, watchpoint.id),
        renderer.paint(Role::Name, watch_subject(watchpoint)),
        plural(watchpoint.byte_size, "byte"),
        renderer.paint(Role::Metadata, watchpoint.address),
        plural(watchpoint.coverage.len() as u64, "hardware slot"),
        watch_scope_suffix(&watchpoint.scope),
    )
}

pub fn watchpoints(watchpoints: &[Watchpoint], renderer: Renderer) -> String {
    lines_or(watchpoints, "no watchpoints", renderer, |watchpoint| {
        format!(
            "{}  {}  {}  {} at {}{}",
            renderer.paint(Role::Metadata, watchpoint.id),
            watchpoint.access,
            renderer.paint(Role::Name, watch_subject(watchpoint)),
            plural(watchpoint.byte_size, "byte"),
            renderer.paint(Role::Metadata, watchpoint.address),
            watch_scope_suffix(&watchpoint.scope),
        )
    })
}

fn watchpoint_invalidations(invalidated: &[InvalidatedWatchpoint], renderer: Renderer) -> String {
    invalidated
        .iter()
        .map(|entry| {
            format!(
                "{} {} {}: {}",
                renderer.paint(Role::Warning, "deleted"),
                renderer.paint(
                    Role::Metadata,
                    format!("watchpoint {}", entry.watchpoint.id)
                ),
                renderer.paint(Role::Name, watch_subject(&entry.watchpoint)),
                match entry.reason {
                    WatchpointInvalidation::ScopeExited => "its frame or block is no longer active",
                    WatchpointInvalidation::OwnerThreadExited => "the thread owning it exited",
                    WatchpointInvalidation::ModuleUnloaded => "the module owning it was unloaded",
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Describes each hit with the watched value before and after the access.
pub fn watchpoint_hits(
    hits: &[WatchpointHit],
    watchpoints: &[Watchpoint],
    image: Option<&ModuleImage>,
    renderer: Renderer,
) -> String {
    hits.iter()
        .map(|hit| {
            let watchpoint = watchpoints
                .iter()
                .find(|watchpoint| watchpoint.id == hit.watchpoint);
            let type_info = watchpoint.and_then(|watchpoint| watchpoint.type_info.as_ref());
            let old = value::watched_bytes(hit.previous.as_deref(), type_info, image);
            let new = value::watched_bytes(hit.current.as_deref(), type_info, image);
            format!(
                "{} by {}{} in thread {}{}",
                renderer.paint(Role::Current, "stopped"),
                renderer.paint(Role::Metadata, format!("watchpoint {}", hit.watchpoint)),
                watchpoint.map_or_else(String::new, |watchpoint| format!(
                    " ({}) on {}",
                    watchpoint.access,
                    renderer.paint(Role::Name, watch_subject(watchpoint))
                )),
                renderer.paint(Role::Metadata, hit.thread),
                if hit.changed() {
                    format!("\n  old: {old}\n  new: {new}")
                } else {
                    format!("\n  value: {new} (unchanged)")
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn exception(description: &str, code: u64, renderer: Renderer) -> String {
    format!("{} ({code:#x})", renderer.paint(Role::Error, description))
}

/// Summarizes a stop on one line, without watched values or source.
pub fn stop(reason: &StopReason, renderer: Renderer) -> String {
    let stopped = |role| renderer.paint(role, "stopped");
    match reason {
        StopReason::Attach => format!("{} after attaching", stopped(Role::Current)),
        StopReason::Breakpoint { address } => format!(
            "{} at breakpoint {}",
            stopped(Role::Current),
            renderer.paint(Role::Metadata, address)
        ),
        StopReason::Watchpoint { hits } => format!(
            "{} by watchpoint {}",
            stopped(Role::Current),
            renderer.paint(
                Role::Metadata,
                hits.iter()
                    .map(|hit| hit.watchpoint.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        ),
        StopReason::WatchpointInvalidated { invalidated } => {
            watchpoint_invalidations(invalidated, renderer)
        }
        StopReason::WatchpointArmFailed {
            thread_id,
            description,
        } => format!(
            "{} new thread {} before it ran: watchpoints could not be armed: {description}",
            stopped(Role::Error),
            renderer.paint(Role::Metadata, thread_id)
        ),
        StopReason::Step { kind } => format!(
            "{} after {}",
            stopped(Role::Current),
            match kind {
                StepKind::Instruction => "instruction step",
                StepKind::IntoSource => "source step",
                StepKind::OverSource => "source next",
                StepKind::Out => "frame return",
            }
        ),
        StopReason::Pause => format!("inferior {}", renderer.paint(Role::Current, "paused")),
        StopReason::Exception(info) => format!(
            "{} by {}",
            stopped(Role::Error),
            exception(&info.description, info.code, renderer)
        ),
        StopReason::Exec => format!(
            "inferior {} its executable image",
            renderer.paint(Role::Warning, "replaced")
        ),
        StopReason::ThreadExited { thread_id, status } => format!(
            "thread {} {}: {}",
            renderer.paint(Role::Metadata, thread_id),
            renderer.paint(Role::Warning, "exited"),
            match status {
                ExitStatus::Code(code) => format!("status {}", exit_code(*code, renderer)),
                ExitStatus::Terminated(info) => exception(&info.description, info.code, renderer),
            }
        ),
        StopReason::CoreDump { exception: None } => format!(
            "{} without a recorded signal",
            renderer.paint(Role::Warning, "dumped")
        ),
        StopReason::CoreDump {
            exception: Some(info),
        } => format!(
            "process {} by {}",
            renderer.paint(Role::Error, "terminated"),
            exception(&info.description, info.code, renderer)
        ),
        StopReason::Unclassifiable { description } => format!(
            "inferior {} for an unclassifiable reason: {description}",
            stopped(Role::Error)
        ),
        StopReason::Exited(ExitStatus::Code(code)) => format!(
            "inferior {} with status {}",
            renderer.paint(exit_role(*code), "exited"),
            exit_code(*code, renderer)
        ),
        StopReason::Exited(ExitStatus::Terminated(info)) => format!(
            "inferior {} by {}",
            renderer.paint(Role::Error, "terminated"),
            exception(&info.description, info.code, renderer)
        ),
    }
}

const fn exit_role(code: i64) -> Role {
    if code == 0 {
        Role::Success
    } else {
        Role::Error
    }
}

fn exit_code(code: i64, renderer: Renderer) -> String {
    renderer.paint(exit_role(code), code).to_string()
}

pub fn threads(snapshot: &StateSnapshot, renderer: Renderer) -> String {
    snapshot
        .threads
        .iter()
        .map(|thread| {
            let marker = if snapshot.selected_thread == Some(thread.id) {
                renderer.paint(Role::Current, "*").to_string()
            } else {
                " ".to_owned()
            };
            let state = match &thread.state {
                ThreadState::Running => "running".to_owned(),
                ThreadState::Stopped {
                    reason: Some(reason),
                } => format!("stopped: {}", stop(reason, renderer)),
                ThreadState::Stopped { reason: None } => "stopped".to_owned(),
            };
            format!(
                "{marker} {} {state}",
                renderer.paint(Role::Metadata, thread.id)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn registers(registers: &RegisterSnapshot, renderer: Renderer) -> String {
    let name_width = registers
        .registers
        .iter()
        .map(|value| value.register.name.len())
        .max()
        .unwrap_or(0);
    registers
        .registers
        .iter()
        .map(|value| {
            format!(
                "{} {}",
                renderer.paint(
                    Role::Name,
                    format_args!("{:<name_width$}", value.register.name)
                ),
                renderer.paint(
                    Role::Value,
                    register_bytes(&value.bytes, registers.target.byte_order)
                )
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Renders the program counter of a register snapshot.
pub fn program_counter(registers: &RegisterSnapshot) -> Option<String> {
    registers
        .registers
        .iter()
        .find(|value| value.register.role == Some(RegisterRole::ProgramCounter))
        .map(|value| register_bytes(&value.bytes, registers.target.byte_order))
}

/// Renders target-order bytes as one hexadecimal number.
pub fn register_bytes(bytes: &[u8], byte_order: ByteOrder) -> String {
    let digits = |bytes: &mut dyn Iterator<Item = &u8>| {
        bytes.fold(String::new(), |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to a String cannot fail");
            output
        })
    };
    let digits = match byte_order {
        ByteOrder::Little => digits(&mut bytes.iter().rev()),
        ByteOrder::Big => digits(&mut bytes.iter()),
    };
    format!("0x{digits}")
}

pub fn memory_read(read: &MemoryRead, renderer: Renderer) -> String {
    let address_width = match read.target.pointer_width {
        PointerWidth::Bits32 => 8,
        PointerWidth::Bits64 => 16,
    };
    let mut lines = Vec::new();
    for (line_index, bytes) in read.bytes.chunks(HEX_DUMP_BYTES_PER_LINE).enumerate() {
        let address = read.address.get() + (line_index * HEX_DUMP_BYTES_PER_LINE) as u64;
        let mut hexadecimal = String::new();
        for index in 0..HEX_DUMP_BYTES_PER_LINE {
            if index == HEX_DUMP_BYTES_PER_LINE / 2 {
                hexadecimal.push(' ');
            }
            match bytes.get(index) {
                Some(byte) => write!(hexadecimal, "{byte:02x} "),
                None => hexadecimal.write_str("   "),
            }
            .expect("writing to a String cannot fail");
        }
        let ascii = (0..HEX_DUMP_BYTES_PER_LINE)
            .map(|index| match bytes.get(index) {
                Some(byte @ 0x20..=0x7e) => char::from(*byte),
                Some(_) => '.',
                None => ' ',
            })
            .collect::<String>();
        lines.push(format!(
            "{}: {} |{}|",
            renderer.paint(Role::Metadata, format!("0x{address:0address_width$x}")),
            renderer.paint(Role::Value, hexadecimal),
            renderer.paint(Role::Value, ascii)
        ));
    }
    if let MemoryReadCompletion::Incomplete {
        next_address,
        reason,
    } = read.completion
    {
        lines.push(
            renderer
                .paint(
                    Role::Warning,
                    format!(
                        "<incomplete: {reason} at 0x{:0address_width$x}; read {} of {} bytes>",
                        next_address.get(),
                        read.bytes.len(),
                        read.requested
                    ),
                )
                .to_string(),
        );
    }
    bound_output(&lines.join("\n"))
}

pub fn source_context(context: &SourceContext, renderer: Renderer) -> String {
    let line_width = context
        .lines
        .last()
        .map_or(1, |line| line.number.to_string().len());
    let mut output = format!(
        "{}:{}",
        renderer.paint(Role::Metadata, context.file.path.display()),
        renderer.paint(Role::Current, context.location.line)
    );
    for line in context.lines.iter() {
        let current = line.number == context.location.line;
        let (marker, role) = if current {
            (
                renderer.paint(Role::Current, "=>").to_string(),
                Role::Current,
            )
        } else {
            ("  ".to_owned(), Role::Metadata)
        };
        write!(
            output,
            "\n{marker} {} | {}",
            renderer.paint(role, format_args!("{:>line_width$}", line.number)),
            line.text
        )
        .expect("writing to a String cannot fail");
    }
    output
}

pub fn core_dump(core: &CoreDumpInfo, renderer: Renderer) -> String {
    let mut lines = vec![
        format!(
            "{} {}",
            renderer.paint(Role::Metadata, "core:"),
            core.path.display()
        ),
        format!(
            "{} {} (process {}): {}",
            renderer.paint(Role::Metadata, "process:"),
            renderer.paint(Role::Name, &core.process_name),
            core.process_id,
            core.arguments
        ),
        format!(
            "{} {}",
            renderer.paint(Role::Metadata, "signal:"),
            core.exception
                .as_ref()
                .map_or("none recorded", |exception| exception.description.as_ref())
        ),
    ];
    for module in core.modules.iter() {
        let state = match &module.state {
            CoreModuleState::Loaded { module, identity } => {
                let identity = match identity {
                    ModuleIdentity::BuildId => "verified by build-id".to_owned(),
                    ModuleIdentity::SavedContent { compared_bytes } => {
                        format!("verified by {compared_bytes} saved bytes")
                    }
                    ModuleIdentity::Mismatched { detail } => renderer
                        .paint(Role::Warning, format!("mismatched: {detail}"))
                        .to_string(),
                    ModuleIdentity::Unverified => {
                        renderer.paint(Role::Warning, "unverified").to_string()
                    }
                };
                format!("module {} {identity}", module.module.id)
            }
            CoreModuleState::Missing => renderer.paint(Role::Warning, "missing").to_string(),
        };
        lines.push(format!(
            "  {} {} {state}",
            renderer.paint(Role::Metadata, module.start),
            module.recorded_path.display()
        ));
    }
    lines.join("\n")
}

/// Describes every module whose file is not proven to match the dump.
pub fn core_module_warnings(core: &CoreDumpInfo) -> Vec<String> {
    core.modules
        .iter()
        .filter_map(|module| match &module.state {
            CoreModuleState::Missing => Some(format!(
                "{} is missing; its frames and unsaved memory are unavailable",
                module.recorded_path.display()
            )),
            CoreModuleState::Loaded {
                module: loaded,
                identity: ModuleIdentity::Mismatched { detail },
            } => Some(format!(
                "{} does not match the dump ({detail}); using its metadata anyway",
                loaded.path.display()
            )),
            CoreModuleState::Loaded {
                module: loaded,
                identity: ModuleIdentity::Unverified,
            } => Some(format!(
                "{} could not be verified against the dump; using its metadata anyway",
                loaded.path.display()
            )),
            CoreModuleState::Loaded { .. } => None,
        })
        .collect()
}

/// Names the code at a location: the debug-info function when known,
/// otherwise the containing linker symbol and offset, demangled when possible.
pub fn code_name(function: Option<&FunctionInfo>, symbol: Option<&SymbolLocation>) -> String {
    if let Some(function) = function {
        return function.name.to_string();
    }
    let Some(symbol) = symbol else {
        return "<unknown>".to_owned();
    };
    let mut name = symbol
        .demangled_name()
        .unwrap_or_else(|| symbol.name.to_string());
    if symbol.offset != 0 {
        write!(name, "+{:#x}", symbol.offset).expect("writing to a String cannot fail");
    }
    if symbol.provenance == SymbolExtentProvenance::Inferred {
        name.push_str(" (unsized symbol)");
    }
    name
}

/// Returns the file name of a loaded module's image.
pub fn module_name(modules: &LoadedModuleSnapshot, module: ModuleId) -> Option<String> {
    modules
        .modules
        .iter()
        .find(|record| record.module.id == module)
        .and_then(|record| record.path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
}

/// Renders frames with source locations from `images` and module names from
/// `modules` for frames without source.
pub fn backtrace(
    trace: &Backtrace,
    modules: Option<&LoadedModuleSnapshot>,
    images: &BTreeMap<ModuleId, Arc<ModuleImage>>,
    renderer: Renderer,
) -> String {
    let mut lines = Vec::with_capacity(trace.frames.len() + 1);
    for frame in trace.frames.iter() {
        let source = frame.source.as_ref().and_then(|source| {
            images
                .get(&frame.module?)?
                .source_file(source.file)
                .map(|file| format!("{}:{}", file.path.display(), source.line))
        });
        let place = source.map_or_else(
            || {
                frame
                    .module
                    .zip(modules)
                    .and_then(|(module, modules)| module_name(modules, module))
                    .map(|module| format!(" from {}", renderer.paint(Role::Metadata, module)))
                    .unwrap_or_default()
            },
            |source| format!(" at {}", renderer.paint(Role::Metadata, source)),
        );
        lines.push(format!(
            "{} {} in {}{place}",
            renderer.paint(
                if frame.level == 0 {
                    Role::Current
                } else {
                    Role::Metadata
                },
                format_args!("#{:<2}", frame.level)
            ),
            renderer.paint(Role::Metadata, format_args!("{:#018x}", frame.instruction)),
            renderer.paint(
                Role::Name,
                code_name(frame.function.as_ref(), frame.symbol.as_ref())
            ),
        ));
    }
    lines.push(format!(
        "{}: {}",
        renderer.paint(Role::Metadata, "unwind stopped"),
        trace.termination
    ));
    lines.join("\n")
}

pub fn globals(page: &GlobalVariablePage, renderer: Renderer) -> String {
    let mut lines = page
        .variables
        .iter()
        .map(|entry| {
            let type_name = match &entry.variable.type_info {
                uscope::GlobalVariableType::Resolved(type_info) => type_info.name.as_ref(),
                uscope::GlobalVariableType::Unsupported(_) => "<unsupported type>",
                uscope::GlobalVariableType::Malformed(_) => "<malformed type>",
                _ => "<unknown type>",
            };
            format!(
                "{} ({})",
                renderer.paint(Role::Name, &entry.variable.qualified_name),
                renderer.paint(Role::Type, type_name)
            )
        })
        .collect::<Vec<_>>();
    let end = page.offset.saturating_add(page.variables.len() as u64);
    if lines.is_empty() {
        lines.push(
            renderer
                .paint(Role::Metadata, "no matching globals")
                .to_string(),
        );
    } else if end < page.total {
        lines.push(
            renderer
                .paint(
                    Role::Warning,
                    format!(
                        "showing {}..{end} of {}; narrow the filter to see the rest",
                        page.offset, page.total
                    ),
                )
                .to_string(),
        );
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_read(
        address: u64,
        bytes: Vec<u8>,
        requested: u64,
        width: PointerWidth,
    ) -> MemoryRead {
        let next_address = address + bytes.len() as u64;
        MemoryRead {
            revision: 4,
            stop_id: uscope::StopId::new(3),
            target: uscope::TargetDescription {
                architecture: uscope::Architecture::X86_64,
                byte_order: ByteOrder::Little,
                pointer_width: width,
            },
            address: uscope::VirtualAddress::new(address),
            requested,
            completion: if bytes.len() as u64 == requested {
                MemoryReadCompletion::Complete
            } else {
                MemoryReadCompletion::Incomplete {
                    next_address: uscope::VirtualAddress::new(next_address),
                    reason: uscope::MemoryReadUnavailableReason::Inaccessible,
                }
            },
            bytes: bytes.into(),
        }
    }

    #[test]
    fn generated_help_lists_every_command_and_shows_usage_only_for_arguments() {
        let renderer = Renderer::new(false);
        let overview = help(renderer);
        for command in COMMANDS {
            assert!(overview.contains(command.name), "missing {}", command.name);
            let detail = command_help(command, renderer);
            assert!(detail.contains(command.summary));
            assert_eq!(detail.contains("\n  usage:"), command.takes_arguments());
            for alias in command.aliases {
                assert!(
                    overview.contains(alias) && detail.contains(alias),
                    "{alias}"
                );
            }
        }
    }

    #[test]
    fn register_bytes_are_rendered_in_target_byte_order() {
        assert_eq!(
            register_bytes(&[0x78, 0x56, 0x34, 0x12], ByteOrder::Little),
            "0x12345678"
        );
        assert_eq!(
            register_bytes(&[0x12, 0x34, 0x56, 0x78], ByteOrder::Big),
            "0x12345678"
        );
    }

    #[test]
    fn memory_reads_render_canonical_hex_ascii_rows_and_partial_outcomes() {
        let bytes = vec![
            0x20, 0x21, 0x7e, 0x7f, 0x41, 0x00, 0xff, 0x5a, 8, 9, 10, 11, 12, 13, 14, 15, 0x61,
            0x62, 0x63,
        ];
        assert_eq!(
            super::memory_read(
                &memory_read(0x1003, bytes, 20, PointerWidth::Bits64),
                Renderer::new(false)
            ),
            concat!(
                "0x0000000000001003: 20 21 7e 7f 41 00 ff 5a  08 09 0a 0b 0c 0d 0e 0f  | !~.A..Z........|\n",
                "0x0000000000001013: 61 62 63                                          |abc             |\n",
                "<incomplete: memory inaccessible at 0x0000000000001016; read 19 of 20 bytes>"
            )
        );
        assert_eq!(
            super::memory_read(
                &memory_read(1, Vec::new(), 8, PointerWidth::Bits32),
                Renderer::new(false)
            ),
            "<incomplete: memory inaccessible at 0x00000001; read 0 of 8 bytes>"
        );
    }

    #[test]
    fn largest_cli_memory_read_fits_the_output_budget_with_color() {
        let size = super::super::commands::MAX_HEX_DUMP_BYTES;
        let bytes = vec![0; usize::try_from(size).expect("test size fits usize")];
        let read = memory_read(0x1000, bytes, size, PointerWidth::Bits64);
        let rendered = super::memory_read(&read, Renderer::new(true));
        assert!(rendered.len() <= value::OUTPUT_LIMIT);
        assert!(!rendered.contains(value::OUTPUT_TRUNCATION_MARKER));
        assert_eq!(rendered.lines().count(), 512);
    }
}
