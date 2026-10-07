//! Renders debugger replies other than values.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use uscope::{
    AddressDescription, Backtrace, BlockCompletion, BoundaryConflict, BoundaryEvidence, Breakpoint,
    BreakpointLocation, ByteOrder, CodeRole, Condition, ConditionOwner, ContextShortfall,
    CoreDumpInfo, CoreModuleState, DecodedInstruction, DisassembledInstruction, Disassembly,
    DisassemblyBlock, DisassemblyView, ExitStatus, FunctionInfo, FunctionOrigin,
    GlobalVariablePage, HitCondition, IndirectTarget, InstructionContent, InstructionReferenceKind,
    InstructionTokenKind, InvalidatedWatchpoint, LanguageException, LanguageExceptionKind,
    LineNumber, LoadedModuleSnapshot, MemoryRead, MemoryReadCompletion, ModuleId, ModuleIdentity,
    ModuleImage, RegisterSnapshot, SourceContext, SourceLine, StackFrame, StackSegment,
    StateSnapshot, StepKind, StopReason, SymbolExtentProvenance, SymbolLocation, TargetBoundary,
    TaskSnapshot, ThreadActivity, ThreadState, VirtualAddress, WatchScope, Watchpoint,
    WatchpointHit, WatchpointInvalidation,
};

use super::commands::{COMMANDS, CommandSpec, aliases};
use super::terminal::{Renderer, Role};
use super::value::{self, bound_output};

const HEX_DUMP_BYTES_PER_LINE: usize = 16;

/// An expression's error, with a caret line under the text it is about and
/// the hint that rewrites it, when there is one.
pub fn expression_error(text: &str, error: &uscope::ExpressionError) -> String {
    let start = (error.span.start as usize).min(text.len());
    let end = (error.span.end as usize).clamp(start, text.len());
    let column = text.get(..start).map_or(0, |prefix| prefix.chars().count());
    let width = text
        .get(start..end)
        .map_or(0, |span| span.chars().count())
        .max(1);
    let mut output = format!(
        "{}\n    {text}\n    {}{}",
        error.message,
        " ".repeat(column),
        "^".repeat(width)
    );
    if let Some(hint) = &error.hint {
        let _ = write!(output, "\nhint: {hint}");
    }
    output
}

/// Returns `count noun`, adding an `s` unless the count is one.
pub fn plural(count: u64, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

pub fn help(aliases: &BTreeMap<String, String>, renderer: Renderer) -> String {
    let name_width = COMMANDS.iter().map(|command| command.name.len()).max();
    let alias_width = COMMANDS
        .iter()
        .map(|command| alias_list(command).len())
        .max();
    let (name_width, alias_width) = (name_width.unwrap_or(0), alias_width.unwrap_or(0));
    let mut output = "commands:".to_owned();
    for command in COMMANDS {
        let aliases = alias_list(command);
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
    if !aliases.is_empty() {
        let width = aliases.keys().map(String::len).max().unwrap_or(0);
        output.push_str("\n\naliases from the settings:");
        for (alias, expansion) in aliases {
            write!(
                output,
                "\n  {}  {expansion}",
                renderer.paint(Role::Alias, format_args!("{alias:<width$}"))
            )
            .expect("writing to a String cannot fail");
        }
    }
    output.push_str("\n\nUse `help <command>` for aliases and usage.");
    output
}

/// A command's other names, joined.
fn alias_list(command: &CommandSpec) -> String {
    aliases(command).collect::<Vec<_>>().join(", ")
}

pub fn command_help(command: &CommandSpec, renderer: Renderer) -> String {
    let mut output = format!("  {}", command.summary);
    let aliases = alias_list(command);
    if !aliases.is_empty() {
        write!(
            output,
            "\n  {}: {}",
            renderer.paint(Role::Muted, "aliases"),
            renderer.paint(Role::Alias, aliases)
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

/// Describes which hits a breakpoint or watchpoint that counted
/// `hit_count` hits stops at, or nothing when it stops at every hit.
fn hit_condition(
    condition: Option<HitCondition>,
    hit_count: u64,
    renderer: Renderer,
) -> Option<String> {
    let condition = condition?;
    let mut text = format!("stops at hits {}", renderer.paint(Role::Name, condition));
    if !condition.may_stop_after(hit_count) {
        text.push_str(" (no later hit can stop)");
    }
    Some(text)
}

/// Names the breakpoint or watchpoint a condition belongs to.
pub fn condition_owner(owner: ConditionOwner) -> String {
    match owner {
        ConditionOwner::Breakpoint(id) => format!("breakpoint {id}"),
        ConditionOwner::Watchpoint(id) => format!("watchpoint {id}"),
    }
}

pub fn breakpoint(breakpoint: &Breakpoint, placed: &[Placed], renderer: Renderer) -> String {
    let heading = format!(
        "{} {} set",
        renderer.paint(
            Role::Success,
            if breakpoint.temporary {
                "temporary breakpoint"
            } else {
                "breakpoint"
            }
        ),
        renderer.paint(Role::Metadata, breakpoint.id),
    );
    let mut options = String::new();
    let stops = hit_condition(breakpoint.hit_condition, breakpoint.hit_count, renderer);
    match (stops, &breakpoint.condition) {
        (Some(stops), Some(condition)) => write!(
            options,
            ", {stops} where {}",
            renderer.paint(Role::Value, condition)
        ),
        (Some(stops), None) => write!(options, ", {stops}"),
        (None, Some(condition)) => write!(
            options,
            ", stops where {}",
            renderer.paint(Role::Value, condition)
        ),
        (None, None) => Ok(()),
    }
    .expect("writing to a String cannot fail");
    if let Some(message) = &breakpoint.log_message {
        write!(
            options,
            ", logs \"{}\"",
            renderer.paint(Role::Value, message)
        )
        .expect("writing to a String cannot fail");
    }
    if let [only] = placed {
        return format!("{heading} at {}{options}", self::placed(only, renderer));
    }
    let mut output = if placed.is_empty() {
        format!("{heading}, pending until a module with its code loads{options}")
    } else {
        format!("{heading} at {} locations{options}", placed.len())
    };
    for location in placed {
        let described = self::placed(location, renderer);
        // A location without a source line already says its address.
        if location.source.is_none() {
            write!(output, "\n  {described}")
        } else {
            write!(
                output,
                "\n  {}  {described}",
                breakpoint_address(location.location, renderer)
            )
        }
        .expect("writing to a String cannot fail");
    }
    output
}

/// Where one breakpoint location is, as far as its module tells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    pub location: BreakpointLocation,
    /// The innermost function, inline or not, containing it.
    pub function: Option<Arc<str>>,
    pub source: Option<(Arc<PathBuf>, LineNumber)>,
    /// The module containing it, unless it is the program's own.
    pub module: Option<Arc<PathBuf>>,
}

/// `parse_header at parse.c:41 in libparse.so`, saying as much as is known
/// and the address when no source line is.
pub fn placed(placed: &Placed, renderer: Renderer) -> String {
    let mut output = match (&placed.function, &placed.source) {
        (Some(function), Some((path, line))) => format!(
            "{} at {}",
            renderer.paint(Role::Name, function),
            renderer.paint(Role::Metadata, renderer.location(path, line))
        ),
        (None, Some((path, line))) => renderer
            .paint(Role::Metadata, renderer.location(path, line))
            .to_string(),
        (Some(function), None) => format!(
            "{} at {}",
            renderer.paint(Role::Name, function),
            breakpoint_address(placed.location, renderer)
        ),
        (None, None) => breakpoint_location(placed.location, renderer),
    };
    if let Some(module) = &placed.module {
        let name = module.file_name().map_or_else(
            || module.display().to_string(),
            |name| name.display().to_string(),
        );
        write!(output, " in {}", renderer.paint(Role::Name, name))
            .expect("writing to a String cannot fail");
    }
    output
}

fn breakpoint_address(location: BreakpointLocation, renderer: Renderer) -> String {
    match location {
        BreakpointLocation::Image(address) => renderer.paint(Role::Metadata, address).to_string(),
        BreakpointLocation::Virtual(address) => renderer.paint(Role::Metadata, address).to_string(),
    }
}

/// Describes a breakpoint whose hit condition changed.
pub fn breakpoint_hit_condition(breakpoint: &Breakpoint, renderer: Renderer) -> String {
    format!(
        "{} {} {}, hit {} so far",
        renderer.paint(Role::Success, "breakpoint"),
        renderer.paint(Role::Metadata, breakpoint.id),
        hit_condition(breakpoint.hit_condition, breakpoint.hit_count, renderer)
            .unwrap_or_else(|| "stops at every hit".to_owned()),
        plural(breakpoint.hit_count, "time"),
    )
}

/// Describes a watchpoint whose hit condition changed.
pub fn watchpoint_hit_condition(watchpoint: &Watchpoint, renderer: Renderer) -> String {
    format!(
        "{} {} {}, hit {} so far",
        renderer.paint(Role::Success, "watchpoint"),
        renderer.paint(Role::Metadata, watchpoint.id),
        hit_condition(watchpoint.hit_condition, watchpoint.hit_count, renderer)
            .unwrap_or_else(|| "stops at every hit".to_owned()),
        plural(watchpoint.hit_count, "time"),
    )
}

/// A table of breakpoints: whether each is enabled, its hits, where it
/// is, and its options, with several locations listed beneath it.
pub fn breakpoints(rows: &[(&Breakpoint, Vec<Placed>)], renderer: Renderer) -> String {
    if rows.is_empty() {
        return renderer.paint(Role::Metadata, "no breakpoints").to_string();
    }
    let (branch, last) = if renderer.unicode() {
        ("\u{251c} ", "\u{2514} ")
    } else {
        ("|- ", "`- ")
    };
    let mut table = Table::new(&["Id", "On", "Hits", "Where", "Options"], &[2]);
    for (breakpoint, placed) in rows {
        let place = match placed.as_slice() {
            [only] => self::placed(only, renderer),
            [] => renderer.paint(Role::Name, &breakpoint.spec).to_string(),
            several => format!(
                "{}, {} locations",
                renderer.paint(Role::Name, &breakpoint.spec),
                several.len()
            ),
        };
        let mut options = counted_options(
            breakpoint.hit_condition,
            breakpoint.hit_count,
            breakpoint.condition.as_ref(),
            renderer,
        );
        if let Some(message) = &breakpoint.log_message {
            options.push(format!("log \"{}\"", renderer.paint(Role::Value, message)));
        }
        if breakpoint.temporary {
            options.push("temporary".to_owned());
        }
        if placed.is_empty() {
            options.push(renderer.paint(Role::Warning, "pending").to_string());
        }
        table.row(
            vec![
                renderer.paint(Role::Metadata, breakpoint.id).to_string(),
                enabled_mark(breakpoint.enabled, renderer),
                breakpoint.hit_count.to_string(),
                place,
                options.join("  "),
            ],
            if placed.len() > 1 {
                placed
                    .iter()
                    .enumerate()
                    .map(|(index, location)| {
                        format!(
                            "{}{}  {}",
                            if index + 1 == placed.len() {
                                last
                            } else {
                                branch
                            },
                            breakpoint_address(location.location, renderer),
                            self::placed(location, renderer)
                        )
                    })
                    .collect()
            } else {
                Vec::new()
            },
        );
    }
    table.render(3, renderer)
}

/// `+` or `●` for enabled, `-` or `○` for disabled.
fn enabled_mark(enabled: bool, renderer: Renderer) -> String {
    match (enabled, renderer.unicode()) {
        (true, true) => renderer.paint(Role::Success, "\u{25cf}").to_string(),
        (true, false) => renderer.paint(Role::Success, "+").to_string(),
        (false, true) => renderer.paint(Role::Muted, "\u{25cb}").to_string(),
        (false, false) => renderer.paint(Role::Muted, "-").to_string(),
    }
}

/// A breakpoint's or watchpoint's hit condition and condition, as `break`
/// takes them.
fn counted_options(
    hit: Option<HitCondition>,
    hit_count: u64,
    condition: Option<&Condition>,
    renderer: Renderer,
) -> Vec<String> {
    let mut options = Vec::new();
    if let Some(hit) = hit {
        let mut text = format!("hits {}", renderer.paint(Role::Name, hit));
        if !hit.may_stop_after(hit_count) {
            text.push_str(" (no later hit can stop)");
        }
        options.push(text);
    }
    if let Some(condition) = condition {
        options.push(format!("if {}", renderer.paint(Role::Value, condition)));
    }
    options
}

/// Rows of cells padded into columns by their visible width, each with
/// lines beneath it that start where the column `beneath` does.
struct Table {
    headings: Vec<String>,
    /// The columns aligned right, as counts are.
    right: &'static [usize],
    rows: Vec<(Vec<String>, Vec<String>)>,
}

impl Table {
    fn new(headings: &[&str], right: &'static [usize]) -> Self {
        Self {
            headings: headings.iter().map(|&heading| heading.to_owned()).collect(),
            right,
            rows: Vec::new(),
        }
    }

    fn row(&mut self, cells: Vec<String>, beneath: Vec<String>) {
        self.rows.push((cells, beneath));
    }

    fn render(self, beneath: usize, renderer: Renderer) -> String {
        let columns = self.headings.len();
        // A last column every row leaves empty is not shown.
        let shown = if self
            .rows
            .iter()
            .all(|(cells, _)| cells[columns - 1].is_empty())
        {
            columns - 1
        } else {
            columns
        };
        let widths = (0..shown)
            .map(|column| {
                self.rows
                    .iter()
                    .map(|(cells, _)| visible_width(&cells[column]))
                    .chain([self.headings[column].len()])
                    .max()
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>();
        let indent = widths[..beneath]
            .iter()
            .map(|width| width + 2)
            .sum::<usize>();
        let line = |cells: &[String]| {
            let mut line = String::new();
            for (column, cell) in cells.iter().take(shown).enumerate() {
                let padding = " ".repeat(widths[column] - visible_width(cell));
                if column > 0 {
                    line.push_str("  ");
                }
                if self.right.contains(&column) {
                    line.push_str(&padding);
                    line.push_str(cell);
                } else {
                    line.push_str(cell);
                    if column + 1 < shown {
                        line.push_str(&padding);
                    }
                }
            }
            line.trim_end().to_owned()
        };
        let headings = self
            .headings
            .iter()
            .map(|heading| renderer.paint(Role::Muted, heading).to_string())
            .collect::<Vec<_>>();
        let mut lines = vec![line(&headings)];
        for (cells, under) in &self.rows {
            lines.push(line(cells));
            lines.extend(
                under
                    .iter()
                    .map(|text| format!("{}{text}", " ".repeat(indent))),
            );
        }
        lines.join("\n")
    }
}

/// The width a terminal shows `text` in, without its color and link
/// escapes.
fn visible_width(text: &str) -> usize {
    let mut width = 0;
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\u{1b}' {
            width += 1;
            continue;
        }
        match characters.next() {
            // A control sequence ends at its final byte.
            Some('[') => {
                for character in characters.by_ref() {
                    if ('@'..='~').contains(&character) {
                        break;
                    }
                }
            }
            // An operating system command, such as a link, ends at ST.
            Some(']') => {
                while let Some(character) = characters.next() {
                    if character == '\u{7}' {
                        break;
                    }
                    if character == '\u{1b}' && characters.peek() == Some(&'\\') {
                        characters.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    width
}

/// Names a watchpoint by the expression that resolved it, or by its bytes.
pub fn watch_subject(watchpoint: &Watchpoint) -> String {
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
        // The watch follows the task's stack wherever its runtime moves it.
        WatchScope::Task { task, activation } => {
            format!(" (frame {activation:#x} below the top of task {task}'s stack)")
        }
    }
}

pub fn watchpoint_set(watchpoint: &Watchpoint, renderer: Renderer) -> String {
    let mut output = format!(
        "{} {} set on {}: {} at {} using {}{}",
        renderer.paint(Role::Success, "watchpoint"),
        renderer.paint(Role::Metadata, watchpoint.id),
        renderer.paint(Role::Name, watch_subject(watchpoint)),
        plural(watchpoint.byte_size, "byte"),
        renderer.paint(Role::Metadata, watchpoint.address),
        plural(watchpoint.coverage.len() as u64, "hardware slot"),
        watch_scope_suffix(&watchpoint.scope),
    );
    if let Some(condition) = &watchpoint.condition {
        write!(
            output,
            ", stops where {}",
            renderer.paint(Role::Value, condition)
        )
        .expect("writing to a String cannot fail");
    }
    output
}

/// Describes a watchpoint whose condition changed.
pub fn watchpoint_condition(watchpoint: &Watchpoint, renderer: Renderer) -> String {
    let id = renderer.paint(Role::Metadata, watchpoint.id);
    watchpoint.condition.as_ref().map_or_else(
        || format!("watchpoint {id} stops unconditionally"),
        |condition| {
            format!(
                "watchpoint {id} stops where {} holds",
                renderer.paint(Role::Value, condition)
            )
        },
    )
}

/// A table of watchpoints: whether each is enabled, its hits, what and
/// where it watches, and its options.
pub fn watchpoints(watchpoints: &[Watchpoint], renderer: Renderer) -> String {
    if watchpoints.is_empty() {
        return renderer.paint(Role::Metadata, "no watchpoints").to_string();
    }
    let mut table = Table::new(&["Id", "On", "Hits", "Watching", "Where", "Options"], &[2]);
    for watchpoint in watchpoints {
        let mut options = vec![watchpoint.access.to_string()];
        options.extend(counted_options(
            watchpoint.hit_condition,
            watchpoint.hit_count,
            watchpoint.condition.as_ref(),
            renderer,
        ));
        table.row(
            vec![
                renderer.paint(Role::Metadata, watchpoint.id).to_string(),
                enabled_mark(watchpoint.enabled, renderer),
                watchpoint.hit_count.to_string(),
                renderer
                    .paint(Role::Name, watch_subject(watchpoint))
                    .to_string(),
                format!(
                    "{} at {}{}",
                    plural(watchpoint.byte_size, "byte"),
                    renderer.paint(Role::Metadata, watchpoint.address),
                    watch_scope_suffix(&watchpoint.scope),
                ),
                options.join("  "),
            ],
            Vec::new(),
        );
    }
    table.render(3, renderer)
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
                invalidation_text(entry.reason)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Why a watchpoint was deleted when its storage ended.
pub const fn invalidation_text(reason: WatchpointInvalidation) -> &'static str {
    match reason {
        WatchpointInvalidation::ScopeExited => "its frame or block is no longer active",
        WatchpointInvalidation::OwnerThreadExited => "the thread owning it exited",
        WatchpointInvalidation::ModuleUnloaded => "the module owning it was unloaded",
        WatchpointInvalidation::StackMoved => {
            "its runtime moved its stack where the debugger could not follow"
        }
    }
}

/// Describes each hit with the watched value before and after the access.
/// Renders the watchpoints a stop hit, the first hit's line ending with
/// `first` and each other's naming its thread.
pub fn watchpoint_hits(
    hits: &[WatchpointHit],
    watchpoints: &[Watchpoint],
    image: Option<&ModuleImage>,
    first: &str,
    renderer: Renderer,
) -> String {
    hits.iter()
        .enumerate()
        .map(|(index, hit)| {
            let watchpoint = watchpoints
                .iter()
                .find(|watchpoint| watchpoint.id == hit.watchpoint);
            let type_info = watchpoint.and_then(|watchpoint| watchpoint.type_info.as_ref());
            let old = value::watched_bytes(hit.previous.as_deref(), type_info, image);
            let new = value::watched_bytes(hit.current.as_deref(), type_info, image);
            let thread = if index == 0 {
                first.to_owned()
            } else {
                format!(" in thread {}", renderer.paint(Role::Metadata, hit.thread))
            };
            format!(
                "{} by {}{}{thread}{}",
                renderer.paint(Role::Current, "stopped"),
                renderer.paint(Role::Metadata, format!("watchpoint {}", hit.watchpoint)),
                watchpoint.map_or_else(
                    || format!(" (hit {})", hit.hit_count),
                    |watchpoint| format!(
                        " ({}, hit {}) on {}",
                        watchpoint.access,
                        hit.hit_count,
                        renderer.paint(Role::Name, watch_subject(watchpoint))
                    )
                ),
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

/// Names the breakpoints or watchpoints a stop hit, with each hit's number:
/// `breakpoint 1 (hit 3)` or `watchpoints 1 (hit 2), 2 (hit 5)`.
fn numbered_hits(
    noun: &str,
    hits: impl ExactSizeIterator<Item = (u64, u64)>,
    renderer: Renderer,
) -> String {
    let plural = if hits.len() == 1 { "" } else { "s" };
    let hits = hits
        .map(|(id, hit)| format!("{} (hit {hit})", renderer.paint(Role::Metadata, id)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{noun}{plural} {hits}")
}

/// A runtime's exception, followed by its message as the runtime prints
/// it, which may take several lines.
fn language_exception(raised: &LanguageException, renderer: Renderer) -> String {
    format!(
        "{} {}:\n{}",
        renderer.paint(Role::Error, "stopped"),
        match raised.kind {
            LanguageExceptionKind::Raised => "as an exception was raised",
            LanguageExceptionKind::Unhandled => "by an unhandled exception",
            LanguageExceptionKind::Fatal => "by a fatal runtime error",
        },
        renderer.paint(Role::Error, &raised.message)
    )
}

/// Summarizes a stop on one line, without watched values or source.
pub fn stop(reason: &StopReason, renderer: Renderer) -> String {
    let stopped = |role| renderer.paint(role, "stopped");
    match reason {
        StopReason::Attach => format!("{} after attaching", stopped(Role::Current)),
        StopReason::Entry => format!("{} at the program entry", stopped(Role::Current)),
        StopReason::Breakpoint { hits, .. } => format!(
            "{} at {}",
            stopped(Role::Current),
            numbered_hits(
                "breakpoint",
                hits.iter().map(|hit| (hit.breakpoint.get(), hit.hit_count)),
                renderer
            )
        ),
        StopReason::Watchpoint { hits } => format!(
            "{} by {}",
            stopped(Role::Current),
            numbered_hits(
                "watchpoint",
                hits.iter().map(|hit| (hit.watchpoint.get(), hit.hit_count)),
                renderer
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
        StopReason::Step { kind } => {
            format!("{} after {}", stopped(Role::Current), step_name(*kind))
        }
        StopReason::StepIncomplete { kind, description } => format!(
            "{} before the {} completed: {description}",
            stopped(Role::Warning),
            step_name(*kind)
        ),
        StopReason::Pause => format!("inferior {}", renderer.paint(Role::Current, "paused")),
        StopReason::Exception(info) => format!(
            "{} by {}",
            stopped(Role::Error),
            exception(&info.description, info.code, renderer)
        ),
        StopReason::LanguageException(raised) => language_exception(raised, renderer),
        StopReason::ProgramBreakpoint { address } => format!(
            "{} by the program's breakpoint instruction at {}",
            stopped(Role::Current),
            renderer.paint(Role::Metadata, address)
        ),
        StopReason::Exec { followed } => format!(
            "inferior {} its executable image",
            renderer.paint(
                Role::Warning,
                if *followed { "re-executed" } else { "replaced" }
            )
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

const fn step_name(kind: StepKind) -> &'static str {
    match kind {
        StepKind::Instruction => "instruction step",
        StepKind::OverInstruction => "instruction next",
        StepKind::IntoSource => "source step",
        StepKind::OverSource => "source next",
        StepKind::Out => "frame return",
        StepKind::Advance => "advance",
        StepKind::IntoNewTask => "new task step",
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
            let marker = if snapshot.selected == Some(uscope::ExecutionContext::Thread(thread.id)) {
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
            let name = thread
                .name
                .as_ref()
                .map(|name| {
                    format!(
                        " {}",
                        renderer.paint(Role::Name, format_args!("\"{name}\""))
                    )
                })
                .unwrap_or_default();
            let activity = match &thread.activity {
                Some(ThreadActivity::Task { task, stack }) => {
                    let place = match stack {
                        StackSegment::System => " on its runtime's stack",
                        StackSegment::Signal => " on its signal stack",
                        _ => "",
                    };
                    format!(
                        " — {}{place}",
                        renderer.paint(Role::Metadata, format_args!("[{}]", task.number))
                    )
                }
                Some(ThreadActivity::Idle) => " — idle".to_owned(),
                _ => String::new(),
            };
            format!(
                "{marker} {}{name} {state}{activity}",
                renderer.paint(Role::Metadata, thread.id)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One task: its id, where the code the program wrote has it, what it does
/// in its runtime's words, and the thread it is on.
pub fn task(task: &TaskSnapshot, place: &str, selected: bool, renderer: Renderer) -> String {
    let marker = if selected {
        renderer.paint(Role::Current, "*").to_string()
    } else {
        " ".to_owned()
    };
    let detail = task
        .detail
        .as_deref()
        .map(|detail| format!(" — {detail}"))
        .unwrap_or_default();
    let thread = task
        .thread
        .map(|thread| format!(" (thread {})", renderer.paint(Role::Metadata, thread)))
        .unwrap_or_default();
    let labels = task_labels(task)
        .map(|labels| format!(" {}", renderer.paint(Role::Name, labels)))
        .unwrap_or_default();
    format!(
        "{marker} {} {place}{detail}{labels}{thread}",
        renderer.paint(Role::Metadata, format_args!("[{}]", task.id.number))
    )
}

/// A task's labels as Go's tracebacks show them, `{job: resize, user: "a
/// b"}`, quoting a key or value only where it needs it; `None` without
/// labels.
pub fn task_labels(task: &TaskSnapshot) -> Option<String> {
    let quoted = |text: &str| {
        if !text.is_empty()
            && text
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/'))
        {
            text.to_owned()
        } else {
            format!("{text:?}")
        }
    };
    (!task.labels.is_empty()).then(|| {
        let pairs = task
            .labels
            .iter()
            .map(|(key, value)| format!("{}: {}", quoted(key), quoted(value)))
            .collect::<Vec<_>>();
        format!("{{{}}}", pairs.join(", "))
    })
}

/// The function a task's place names: the innermost the program wrote, or
/// for a task of only the runtime's code, the one it began in.
pub fn task_function(task: &TaskSnapshot, trace: &Backtrace) -> Option<String> {
    trace.user_frame().map_or_else(
        || {
            task.entry
                .as_ref()
                .and_then(|entry| entry.function.as_deref())
                .map(str::to_owned)
        },
        |frame| Some(code_name(frame.function.as_ref(), frame.symbol.as_ref())),
    )
}

/// Where a task is: the code the program wrote that it runs, or the
/// function a task of only the runtime's code began in, or why its frames
/// are unknown.
pub fn task_place(
    task: &TaskSnapshot,
    trace: &uscope::Result<Backtrace>,
    images: &BTreeMap<ModuleId, Arc<ModuleImage>>,
    renderer: Renderer,
) -> String {
    match trace {
        Ok(trace) => trace.user_frame().map_or_else(
            || {
                task_function(task, trace).map_or_else(
                    || renderer.paint(Role::Metadata, "<runtime code>").to_string(),
                    |entry| renderer.paint(Role::Name, entry).to_string(),
                )
            },
            |frame| {
                let name = renderer.paint(
                    Role::Name,
                    code_name(frame.function.as_ref(), frame.symbol.as_ref()),
                );
                frame_source(frame, images, renderer).map_or_else(
                    || name.to_string(),
                    |source| format!("{name} at {}", renderer.paint(Role::Metadata, source)),
                )
            },
        ),
        Err(error) => renderer
            .paint(Role::Metadata, format_args!("<{error}>"))
            .to_string(),
    }
}

/// Tasks that are in one place.
pub fn task_group(noun: &str, place: &str, numbers: &[u64], renderer: Renderer) -> String {
    let listed = numbers
        .iter()
        .map(|number| renderer.paint(Role::Metadata, number).to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{} in {place}: {listed}",
        plural(numbers.len() as u64, noun)
    )
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
                value.bytes.as_ref().map_or_else(
                    || renderer.paint(Role::Muted, "<not saved>").to_string(),
                    |bytes| renderer
                        .paint(
                            Role::Value,
                            register_bytes(bytes, registers.target.byte_order)
                        )
                        .to_string()
                )
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
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
    let address_width = usize::from(read.target.pointer_width.bytes()) * 2;
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

/// Renders source lines around a location, after the location itself when
/// `located`, with a margin marking each line in `breakpoints`, enabled or
/// not, when any shown line has one, and each line's text as `text` draws
/// it.
pub fn source_context(
    context: &SourceContext,
    breakpoints: &BTreeMap<LineNumber, bool>,
    located: bool,
    text: &dyn Fn(&SourceLine) -> String,
    renderer: Renderer,
) -> String {
    let line_width = context
        .lines
        .last()
        .map_or(1, |line| line.number.to_string().len());
    let margin = context
        .lines
        .iter()
        .any(|line| breakpoints.contains_key(&line.number));
    let mut lines = Vec::with_capacity(context.lines.len() + 1);
    if located {
        lines.push(format!(
            "{}:{}",
            renderer.paint(Role::Metadata, renderer.path(&context.path)),
            renderer.paint(Role::Current, context.location.line)
        ));
    }
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
        let breakpoint = match breakpoints.get(&line.number) {
            Some(enabled) => enabled_mark(*enabled, renderer),
            None if margin => " ".to_owned(),
            None => String::new(),
        };
        lines.push(format!(
            "{breakpoint}{marker} {} | {}",
            renderer.paint(role, format_args!("{:>line_width$}", line.number)),
            text(line)
        ));
    }
    lines.join("\n")
}

/// One module `info modules` lists.
pub struct ModuleRow {
    pub path: Arc<PathBuf>,
    /// Where it is loaded, once it is.
    pub load_bias: Option<u64>,
    pub image: Option<Arc<uscope::ModuleImage>>,
}

/// The modules, one per line with the range each occupies and what
/// describes its code, and below one stripped of its debug information the
/// separate file that holds it, or why that file could not be used.
pub fn modules(modules: &[ModuleRow], renderer: Renderer) -> String {
    let mut lines = Vec::with_capacity(modules.len());
    for module in modules {
        let range = match (&module.image, module.load_bias) {
            (Some(image), Some(bias)) => {
                let range = image.address_range();
                format!(
                    "{:#x}-{:#x}",
                    range.start.get().wrapping_add(bias),
                    range.end.get().wrapping_add(bias)
                )
            }
            (_, None) => "not loaded".to_owned(),
            (None, Some(_)) => "?".to_owned(),
        };
        let described = match &module.image {
            Some(image) if !image.functions().is_empty() => "debug",
            Some(image) if !image.symbols().is_empty() => "symbols",
            _ => "none",
        };
        lines.push(format!(
            "{}  {described:<7}  {}",
            renderer.paint(Role::Metadata, format!("{range:<29}")),
            renderer.path(&module.path)
        ));
        match module
            .image
            .as_ref()
            .and_then(|image| image.separate_debug_file())
        {
            Some(uscope::DebugFile::Used(path)) => lines.push(format!(
                "  {} {}",
                renderer.paint(Role::Muted, "debug information from"),
                renderer.path(path)
            )),
            Some(uscope::DebugFile::Unusable { path, reason }) => lines.push(format!(
                "  {} {}: {reason}",
                renderer.paint(Role::Warning, "cannot use the debug file"),
                renderer.path(path)
            )),
            None => {}
        }
    }
    lines.join("\n")
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
    for core_module in core.modules.iter() {
        let state = match &core_module.state {
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
                    ModuleIdentity::DumpedMemory => "read from the dump".to_owned(),
                };
                // A file found under a sysroot or in a module path is named.
                let file = if module.path == core_module.recorded_path {
                    String::new()
                } else {
                    format!(" from {}", module.path.display())
                };
                format!("module {}{file} {identity}", module.module.id)
            }
            CoreModuleState::Missing => {
                let build_id = core_module
                    .build_id
                    .as_deref()
                    .map(|build_id| format!(" (build-id {})", build_id_text(build_id)))
                    .unwrap_or_default();
                renderer
                    .paint(Role::Warning, format!("missing{build_id}"))
                    .to_string()
            }
        };
        lines.push(format!(
            "  {} {} {state}",
            renderer.paint(Role::Metadata, core_module.start),
            core_module.recorded_path.display()
        ));
    }
    lines.join("\n")
}

fn build_id_text(build_id: &[u8]) -> String {
    build_id.iter().fold(String::new(), |mut text, byte| {
        write!(text, "{byte:02x}").expect("writing to a String cannot fail");
        text
    })
}

/// Describes every module whose file is not proven to match the dump.
pub fn core_module_warnings(core: &CoreDumpInfo) -> Vec<String> {
    core.modules
        .iter()
        .filter_map(|module| match &module.state {
            CoreModuleState::Missing => Some(format!(
                "{} is missing; its frames and unsaved memory are unavailable{}",
                module.recorded_path.display(),
                module
                    .build_id
                    .as_deref()
                    .map(|build_id| format!(" (build-id {})", build_id_text(build_id)))
                    .unwrap_or_default()
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

/// Renders what contains an address: its symbol and offset, its section, and
/// its module. A data symbol names only bytes within its declared size.
pub fn address_description(description: &AddressDescription, renderer: Renderer) -> String {
    let Some(module) = &description.module else {
        return format!(
            "no loaded module contains {}",
            renderer.paint(Role::Metadata, description.address)
        );
    };
    let place = module.image.section.as_ref().map_or_else(
        || {
            format!(
                "of {}",
                renderer.paint(Role::Metadata, module.path.display())
            )
        },
        |section| {
            format!(
                "in section {} of {}",
                renderer.paint(Role::Metadata, &section.name),
                renderer.paint(Role::Metadata, module.path.display())
            )
        },
    );
    module.image.symbol.as_ref().map_or_else(
        || {
            format!(
                "no symbol contains {} {place}",
                renderer.paint(Role::Metadata, description.address)
            )
        },
        |symbol| {
            format!(
                "{} {place}",
                renderer.paint(Role::Name, code_name(None, Some(symbol)))
            )
        },
    )
}

/// Returns every module whose instructions a disassembly holds.
pub fn disassembly_modules(disassembly: &Disassembly) -> BTreeSet<ModuleId> {
    let blocks = match &disassembly.view {
        DisassemblyView::Function { blocks, .. } => blocks,
        DisassemblyView::Window { block, .. } => std::slice::from_ref(block),
    };
    blocks
        .iter()
        .flat_map(|block| block.instructions.iter())
        .filter_map(|instruction| instruction.location.module.as_ref())
        .map(|module| module.module)
        .collect()
}

/// Renders a disassembly: one line per instruction with its address, its
/// offset from the containing symbol, its bytes, and its text, marking the
/// stopped instruction, naming encoded addresses, and announcing each new
/// source line. Unproven context, conflicts, and unreadable or truncated
/// code are reported where they occur.
pub fn disassembly(
    disassembly: &Disassembly,
    program_counter: Option<VirtualAddress>,
    modules: &LoadedModuleSnapshot,
    images: &BTreeMap<ModuleId, Arc<ModuleImage>>,
    show_bytes: bool,
    renderer: Renderer,
) -> String {
    let mut lines = Vec::new();
    match &disassembly.view {
        DisassemblyView::Function { function, blocks } => {
            let origin = match function.origin {
                FunctionOrigin::DebugInfo { .. } => "",
                FunctionOrigin::Symbol {
                    provenance: SymbolExtentProvenance::Declared,
                    ..
                } => " (symbol)",
                FunctionOrigin::Symbol { .. } => " (unsized symbol)",
            };
            let module = module_name(modules, function.module)
                .map(|name| format!(" in {}", renderer.paint(Role::Metadata, name)))
                .unwrap_or_default();
            lines.push(format!(
                "function {}{origin}{module}:",
                renderer.paint(
                    Role::Name,
                    function
                        .demangled_name()
                        .unwrap_or_else(|| function.name.to_string())
                )
            ));
            for (index, block) in blocks.iter().enumerate() {
                if blocks.len() > 1 {
                    lines.push(format!(
                        "range {} of {}: {}..{}",
                        index + 1,
                        blocks.len(),
                        renderer.paint(Role::Metadata, block.range.start),
                        renderer.paint(Role::Metadata, block.range.end)
                    ));
                }
                disassembly_block(
                    &mut lines,
                    block,
                    program_counter,
                    modules,
                    images,
                    show_bytes,
                    renderer,
                );
            }
        }
        DisassemblyView::Window {
            address,
            boundary,
            leading,
            block,
        } => {
            if let Some(note) = target_boundary_note(*address, *boundary) {
                lines.push(renderer.paint(Role::Warning, note).to_string());
            }
            if let Some(shortfall) = leading {
                lines.push(
                    renderer
                        .paint(Role::Muted, context_shortfall_note(*shortfall))
                        .to_string(),
                );
            }
            disassembly_block(
                &mut lines,
                block,
                program_counter,
                modules,
                images,
                show_bytes,
                renderer,
            );
        }
    }
    lines.join("\n")
}

/// The widest instruction encoding whose bytes keep the text column aligned;
/// longer encodings push their text right.
const ALIGNED_INSTRUCTION_BYTES: usize = 8;

fn disassembly_block(
    lines: &mut Vec<String>,
    block: &DisassemblyBlock,
    program_counter: Option<VirtualAddress>,
    modules: &LoadedModuleSnapshot,
    images: &BTreeMap<ModuleId, Arc<ModuleImage>>,
    show_bytes: bool,
    renderer: Renderer,
) {
    let place = |instruction: &DisassembledInstruction| {
        instruction
            .location
            .module
            .as_ref()
            .and_then(|module| module.image.symbol.as_ref())
            .map_or_else(
                || ":".to_owned(),
                |symbol| format!(" <{}>:", code_name(None, Some(symbol))),
            )
    };
    let place_width = block
        .instructions
        .iter()
        .map(|instruction| place(instruction).len())
        .max()
        .unwrap_or_default();
    let bytes_width = bytes_column_width(block);

    let mut source = None;
    for instruction in block.instructions.iter() {
        let module = instruction.location.module.as_ref();
        let current_source = instruction.source.as_ref().and_then(|location| {
            let file = images.get(&module?.module)?.source_file(location.file)?;
            Some(renderer.location(&file.path, location.line))
        });
        if current_source.is_some() && current_source != source {
            lines.push(
                renderer
                    .paint(
                        Role::Metadata,
                        current_source.as_deref().unwrap_or_default(),
                    )
                    .to_string(),
            );
        }
        source = current_source;

        let marker = if Some(instruction.address) == program_counter {
            renderer.paint(Role::Current, "=>").to_string()
        } else {
            "  ".to_owned()
        };
        let bytes = instruction_bytes(&instruction.bytes, show_bytes.then_some(bytes_width));
        let text = match &instruction.content {
            InstructionContent::Decoded(decoded) => instruction_text(
                decoded,
                Some(instruction.address) == program_counter,
                module.map(|module| module.module),
                modules,
                renderer,
            ),
            InstructionContent::Invalid => renderer.paint(Role::Warning, "(bad)").to_string(),
            InstructionContent::Truncated => renderer
                .paint(Role::Warning, "(truncated by unreadable memory)")
                .to_string(),
        };
        lines.push(format!(
            "{marker} {}{}{bytes} {text}",
            renderer.paint(
                Role::Metadata,
                format_args!("{:#018x}", instruction.address)
            ),
            renderer.paint(Role::Name, format!("{:<place_width$}", place(instruction))),
        ));
        for conflict in block
            .conflicts
            .iter()
            .filter(|conflict| conflict.instruction == instruction.address)
        {
            lines.push(
                renderer
                    .paint(Role::Warning, conflict_note(conflict))
                    .to_string(),
            );
        }
    }
    match block.completion {
        BlockCompletion::Complete => {}
        BlockCompletion::Unreadable { address, reason } => lines.push(
            renderer
                .paint(Role::Warning, format!("{reason} at {address}"))
                .to_string(),
        ),
        BlockCompletion::Limited { next } => lines.push(
            renderer
                .paint(
                    Role::Muted,
                    format!(
                        "stopped at the instruction limit; continue with `disassemble {next} <instruction-count>`"
                    ),
                )
                .to_string(),
        ),
    }
}

/// The width of the bytes column, which fits all but the longest encodings.
fn bytes_column_width(block: &DisassemblyBlock) -> usize {
    block
        .instructions
        .iter()
        .map(|instruction| instruction.bytes.len().min(ALIGNED_INSTRUCTION_BYTES) * 3)
        .max()
        .unwrap_or_default()
}

fn conflict_note(conflict: &BoundaryConflict) -> String {
    if conflict.evidence == BoundaryEvidence::RangeEnd {
        format!(
            "   this instruction extends past the end of the range at {}",
            conflict.boundary
        )
    } else {
        format!(
            "   this instruction overlaps {}, where {} proves an instruction begins; decoding resumes there",
            conflict.boundary, conflict.evidence
        )
    }
}

/// An instruction's bytes, padded to the column's width after a space, or
/// nothing when the column is hidden.
fn instruction_bytes(bytes: &[u8], width: Option<usize>) -> String {
    let Some(width) = width else {
        return String::new();
    };
    let text = bytes.iter().fold(String::new(), |mut text, byte| {
        write!(text, "{byte:02x} ").expect("writing to a String cannot fail");
        text
    });
    format!(" {text:<width$}")
}

/// Renders an instruction's text with each encoded address named.
pub fn instruction_text(
    decoded: &DecodedInstruction,
    stopped: bool,
    module: Option<ModuleId>,
    modules: &LoadedModuleSnapshot,
    renderer: Renderer,
) -> String {
    let mut text = String::new();
    for token in decoded.tokens.iter() {
        let role = match token.kind {
            InstructionTokenKind::Mnemonic | InstructionTokenKind::Prefix => Some(Role::Command),
            InstructionTokenKind::Register => Some(Role::Type),
            InstructionTokenKind::Number | InstructionTokenKind::Address => Some(Role::Value),
            _ => None,
        };
        match role {
            Some(role) => write!(text, "{}", renderer.paint(role, &token.text)),
            None => write!(text, "{}", token.text),
        }
        .expect("writing to a String cannot fail");
    }
    let slot = match decoded.indirect_target.as_deref() {
        Some(IndirectTarget::Memory { slot, .. } | IndirectTarget::Unreadable { slot, .. }) => {
            Some(slot.address)
        }
        _ => None,
    };
    for reference in decoded.references.iter() {
        // The indirect target below names the memory holding it.
        if reference.kind == InstructionReferenceKind::MemoryOperand
            && Some(reference.address) == slot
        {
            continue;
        }
        let Some(name) = reference_name(&reference.description, module, modules) else {
            continue;
        };
        let name = renderer.paint(Role::Name, format!("<{name}>"));
        match reference.kind {
            InstructionReferenceKind::BranchTarget => write!(text, " {name}"),
            InstructionReferenceKind::MemoryOperand => write!(
                text,
                "  {} {name}",
                renderer.paint(Role::Muted, format_args!("# {}", reference.address))
            ),
        }
        .expect("writing to a String cannot fail");
    }
    if let Some(target) = decoded.indirect_target.as_deref() {
        indirect_target_text(&mut text, target, stopped, module, modules, renderer);
    }
    text
}

/// Appends where an indirect branch transfers control at the stop: `# slot
/// <name> -> target <name>` for a target loaded from memory, and `# ->
/// target <name>` for one held in a register. A target that needs registers
/// is noted only at the stopped instruction, since every return elsewhere
/// needs them.
fn indirect_target_text(
    text: &mut String,
    target: &IndirectTarget,
    stopped: bool,
    module: Option<ModuleId>,
    modules: &LoadedModuleSnapshot,
    renderer: Renderer,
) {
    let described = |description: &AddressDescription| {
        let mut text = renderer.paint(Role::Muted, description.address).to_string();
        if let Some(name) = reference_name(description, module, modules) {
            write!(text, " {}", renderer.paint(Role::Name, format!("<{name}>")))
                .expect("writing to a String cannot fail");
        }
        text
    };
    let arrow = renderer.paint(Role::Muted, "->");
    match target {
        IndirectTarget::Register { target } => {
            write!(
                text,
                "  {} {arrow} {}",
                renderer.paint(Role::Muted, "#"),
                described(target)
            )
        }
        IndirectTarget::Memory { slot, target } => write!(
            text,
            "  {} {} {arrow} {}",
            renderer.paint(Role::Muted, "#"),
            described(slot),
            described(target)
        ),
        IndirectTarget::Unreadable {
            slot,
            address,
            reason,
        } => write!(
            text,
            "  {} {} {arrow} {}",
            renderer.paint(Role::Muted, "#"),
            described(slot),
            renderer.paint(Role::Warning, format!("{reason} at {address}"))
        ),
        IndirectTarget::NeedsRegisters if stopped => write!(
            text,
            "  {} {arrow} {}",
            renderer.paint(Role::Muted, "#"),
            renderer.paint(
                Role::Warning,
                "unknown: a restarted system call replaces a register it uses"
            )
        ),
        IndirectTarget::Unsupported => write!(
            text,
            "  {} {arrow} {}",
            renderer.paint(Role::Muted, "#"),
            renderer.paint(Role::Warning, "not computed for this form of branch")
        ),
        _ => Ok(()),
    }
    .expect("writing to a String cannot fail");
}

/// Names an encoded address by its symbol or, failing that, its section,
/// adding the module when it differs from the instruction's.
pub fn reference_name(
    description: &AddressDescription,
    from: Option<ModuleId>,
    modules: &LoadedModuleSnapshot,
) -> Option<String> {
    let module = description.module.as_ref()?;
    let mut name = match (&module.image.symbol, &module.image.section) {
        (Some(symbol), _) => code_name(None, Some(symbol)),
        (None, Some(section)) => format!("{}+{:#x}", section.name, section.offset),
        (None, None) => return None,
    };
    if Some(module.module) != from
        && let Some(file) = module_name(modules, module.module)
    {
        write!(name, " in {file}").expect("writing to a String cannot fail");
    }
    Some(name)
}

fn target_boundary_note(address: VirtualAddress, boundary: TargetBoundary) -> Option<String> {
    match boundary {
        TargetBoundary::Known(_) | TargetBoundary::Reached { .. } => None,
        TargetBoundary::Crossed { from, instruction } => Some(format!(
            "{address} lies inside the instruction at {instruction} when decoding from {from}; it is probably not an instruction start"
        )),
        TargetBoundary::Unverified(shortfall) => Some(format!(
            "{address} is not proven to begin an instruction: {}",
            shortfall_reason(shortfall)
        )),
    }
}

fn context_shortfall_note(shortfall: ContextShortfall) -> String {
    format!(
        "no earlier instructions are shown: {}",
        shortfall_reason(shortfall)
    )
}

fn shortfall_reason(shortfall: ContextShortfall) -> String {
    match shortfall {
        ContextShortfall::NoKnownBoundary => {
            "no known instruction start precedes it closely enough".to_owned()
        }
        ContextShortfall::Desynchronized { boundary } => {
            format!("decoding from the preceding known instruction start overlaps {boundary}")
        }
        ContextShortfall::Unreadable { address } => {
            format!("memory is unreadable at {address}")
        }
    }
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

/// Renders a backtrace, highlighting the selected frame's level.
/// Renders a backtrace, or its first `limit` frames and how many more
/// there are.
pub fn backtrace(
    trace: &Backtrace,
    selected: u32,
    limit: Option<usize>,
    modules: Option<&LoadedModuleSnapshot>,
    images: &BTreeMap<ModuleId, Arc<ModuleImage>>,
    renderer: Renderer,
) -> String {
    let shown = limit.unwrap_or(usize::MAX).min(trace.frames.len());
    let mut lines = Vec::with_capacity(shown + 1);
    // Where a stack continues on another, each run of frames says whose
    // stack it is on.
    let switches = trace
        .frames
        .windows(2)
        .any(|pair| pair[0].segment != pair[1].segment);
    let mut segment = None;
    let iterators = trace.loop_iterators();
    for (frame, iterates) in trace.frames[..shown].iter().zip(iterators) {
        if switches && segment != Some(frame.segment) {
            segment = Some(frame.segment);
            lines.push(
                renderer
                    .paint(
                        Role::Metadata,
                        format_args!("    on {}:", stack_owner(frame.segment)),
                    )
                    .to_string(),
            );
        }
        lines.push(stack_frame(
            frame,
            iterates,
            modules,
            images,
            frame.level == selected,
            renderer,
        ));
    }
    let more = trace.frames.len() - shown;
    lines.push(if more == 0 {
        format!(
            "{}: {}",
            renderer.paint(Role::Metadata, "unwind stopped"),
            trace.termination
        )
    } else {
        renderer
            .paint(
                Role::Muted,
                format!(
                    "{}; `bt` shows every one",
                    plural(more as u64, "more frame")
                ),
            )
            .to_string()
    });
    lines.join("\n")
}

/// Whose stack a run of frames is on.
pub const fn stack_owner(segment: StackSegment) -> &'static str {
    match segment {
        StackSegment::Thread => "the thread's stack",
        StackSegment::Task => "the task's stack",
        StackSegment::System => "the runtime's stack",
        StackSegment::Signal => "the signal stack",
    }
}

/// Renders one backtrace frame: its level, instruction, code, the level of
/// the frame whose loop it iterates, and source location or module.
pub fn stack_frame(
    frame: &StackFrame,
    iterates: Option<u32>,
    modules: Option<&LoadedModuleSnapshot>,
    images: &BTreeMap<ModuleId, Arc<ModuleImage>>,
    selected: bool,
    renderer: Renderer,
) -> String {
    let place = frame_source(frame, images, renderer).map_or_else(
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
    let iterator = iterates.map_or_else(String::new, |level| {
        format!(
            " {}",
            renderer.paint(
                Role::Metadata,
                format_args!("(the iterator of #{level}'s loop)")
            )
        )
    });
    format!(
        "{} {} in {}{iterator}{place}",
        renderer.paint(
            if selected {
                Role::Current
            } else {
                Role::Metadata
            },
            format_args!("#{:<2}", frame.level)
        ),
        renderer.paint(Role::Metadata, format_args!("{:#018x}", frame.instruction)),
        renderer.paint(
            // A runtime's machinery and compiler wrappers recede.
            if frame
                .function
                .as_ref()
                .is_none_or(|function| function.role == CodeRole::Ordinary)
            {
                Role::Name
            } else {
                Role::Metadata
            },
            code_name(frame.function.as_ref(), frame.symbol.as_ref())
        ),
    )
}

/// A frame's source file and line, from its module's image.
fn frame_source(
    frame: &StackFrame,
    images: &BTreeMap<ModuleId, Arc<ModuleImage>>,
    renderer: Renderer,
) -> Option<String> {
    let source = frame.source.as_ref()?;
    images
        .get(&frame.module?)?
        .source_file(source.file)
        .map(|file| renderer.location(&file.path, source.line))
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

/// Renders signal policies as a table, one signal per line.
pub fn signal_policies(policies: &[(u64, uscope::SignalPolicy)], renderer: Renderer) -> String {
    let yes_no = |value: bool| if value { "yes" } else { "no" };
    std::iter::once(
        renderer
            .paint(Role::Muted, "signal    stop  print  pass")
            .to_string(),
    )
    .chain(policies.iter().map(|(code, policy)| {
        format!(
            "{}  {:<4}  {:<5}  {}",
            renderer.paint(
                Role::Name,
                format_args!(
                    "{:<8}",
                    uscope::signal_name(*code).unwrap_or_else(|| code.to_string())
                )
            ),
            yes_no(policy.stop),
            yes_no(policy.print),
            yes_no(policy.pass)
        )
    }))
    .collect::<Vec<_>>()
    .join("\n")
}

/// Says which view presents a value, and why each view tried before it did
/// not bind.
pub fn view_explanation(
    expression: &str,
    explanation: &uscope::ViewExplanation,
    renderer: Renderer,
) -> String {
    let type_name = explanation
        .type_info
        .as_ref()
        .map_or("<unknown type>", |info| &info.name);
    let mut lines = vec![format!(
        "`{expression}` has type {}",
        renderer.paint(Role::Type, type_name)
    )];
    match &explanation.presentation {
        Some(presentation) if presentation.shape == uscope::PresentedShape::Raw => {
            lines.push(format!(
                "{} binds, but shows the value as stored: {}",
                presentation.view,
                presentation
                    .problem
                    .as_ref()
                    .map_or_else(|| "it failed".to_owned(), ToString::to_string)
            ));
        }
        Some(presentation) => {
            lines.push(format!("presented by {}", presentation.view));
            lines.push(format!(
                "as {}",
                renderer.paint(Role::Value, &presentation.summary)
            ));
            if let Some(problem) = &presentation.problem {
                lines.push(format!("the summary stopped short: {problem}"));
            }
        }
        None if !explanation.enabled => lines
            .push("views are off, so it shows as stored; `set views on` turns them on".to_owned()),
        None if explanation.candidates.is_empty() => {
            lines.push("no view's pattern names the type, so it shows as stored".to_owned());
        }
        None => lines.push("no view binds, so it shows as stored".to_owned()),
    }
    if !explanation.candidates.is_empty() {
        lines.push("views tried, in order:".to_owned());
        lines.extend(candidate_lines(&explanation.candidates, "  "));
    }
    lines.join("\n")
}

/// Each view a type was matched against, and why it did not bind or that
/// it did.
fn candidate_lines<'a>(
    candidates: &'a [uscope::ViewCandidate],
    indent: &'a str,
) -> impl Iterator<Item = String> + 'a {
    candidates.iter().map(move |candidate| {
        format!(
            "{indent}{}: {}",
            candidate.view,
            candidate.rejection.as_deref().unwrap_or("binds")
        )
    })
}

/// How each type a name means is presented, as `views explain` says.
pub fn type_views(name: &str, types: &[uscope::TypeViews], renderer: Renderer) -> String {
    if types.is_empty() {
        return format!("no type is named `{name}`");
    }
    let mut lines = Vec::new();
    for views in types {
        lines.push(format!(
            "{} in {}",
            renderer.paint(Role::Type, &views.type_info.name),
            renderer.paint(Role::Metadata, views.module.display())
        ));
        match views.presented_by() {
            Some(view) => lines.push(format!("  presented by {view}")),
            None if views.candidates.is_empty() => {
                lines.push("  no view's pattern names it".to_owned());
            }
            None => lines.push("  no view binds".to_owned()),
        }
        if !views.candidates.is_empty() {
            lines.push("  views tried, in order:".to_owned());
            lines.extend(candidate_lines(&views.candidates, "    "));
        }
    }
    lines.join("\n")
}

/// What `views check` finds, and whether any view loaded for the session
/// or carried by a module presents no type or binds no type it names.
pub fn view_check(check: &uscope::ViewCheck, renderer: Renderer) -> (String, bool) {
    let mut lines = Vec::new();
    let mut failed = false;
    let presented = check
        .types
        .iter()
        .filter_map(|views| Some((views, views.presented_by()?)))
        .collect::<Vec<_>>();
    if !presented.is_empty() {
        lines.push("presented:".to_owned());
        for (views, view) in presented {
            lines.push(format!(
                "  {} by {view}",
                renderer.paint(Role::Type, &views.type_info.name)
            ));
        }
    }
    let refused = check
        .types
        .iter()
        .filter(|views| views.presented_by().is_none())
        .collect::<Vec<_>>();
    if !refused.is_empty() {
        lines.push("not presented, though views name them:".to_owned());
        for views in refused {
            lines.push(format!(
                "  {}",
                renderer.paint(Role::Type, &views.type_info.name)
            ));
            lines.extend(candidate_lines(&views.candidates, "    "));
            failed |= views
                .candidates
                .iter()
                .any(|candidate| !uscope::is_built_in_view(&candidate.view));
        }
    }
    if !check.unused.is_empty() {
        failed = true;
        lines.push("views that present no type:".to_owned());
        lines.extend(check.unused.iter().map(|view| format!("  {view}")));
    }
    if lines.is_empty() {
        lines.push("no view's pattern names any type".to_owned());
    }
    // A kernel is shown as what it is built from, to be reviewed as that.
    if !check.kernels.is_empty() {
        lines.push("kernels, and what they are built from:".to_owned());
        for kernel in check.kernels.iter() {
            lines.push(format!("  {} ({}):", kernel.name, kernel.origin));
            lines.extend(kernel.source.lines().map(|line| {
                if line.is_empty() {
                    String::new()
                } else {
                    format!("    {line}")
                }
            }));
        }
    }
    (lines.join("\n"), failed)
}

/// Renders a logged message with the values it shows.
pub fn log_message(parts: &[uscope::LogPart]) -> String {
    parts
        .iter()
        .map(|part| match part {
            uscope::LogPart::Text(text) => text.to_string(),
            uscope::LogPart::Value {
                type_info, state, ..
            } => value::summary(type_info.as_ref(), state),
            uscope::LogPart::Error { expression, error } => format!("<{expression}: {error}>"),
        })
        .collect()
}

/// Reports a signal that did not stop the inferior.
pub fn signal_received(
    thread: uscope::ThreadId,
    info: &uscope::ExceptionInfo,
    renderer: Renderer,
) -> String {
    format!(
        "thread {} received {}",
        renderer.paint(Role::Metadata, thread),
        exception(&info.description, info.code, renderer)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use uscope::PointerWidth;

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
        let overview = help(&BTreeMap::new(), renderer);
        for command in COMMANDS {
            // Each command has one overview row: its name, aliases, summary.
            let row = overview
                .lines()
                .find(|line| line.split_whitespace().next() == Some(command.name))
                .unwrap_or_else(|| panic!("missing {}:\n{overview}", command.name));
            assert!(row.ends_with(command.summary), "{row}");
            let words = row
                .split_whitespace()
                .map(|word| word.trim_end_matches(','))
                .collect::<Vec<_>>();
            for alias in aliases(command) {
                assert!(words.contains(&alias), "{alias} in {row}");
            }
            let detail = command_help(command, renderer);
            assert!(detail.contains(command.summary));
            assert_eq!(detail.contains("\n  usage:"), command.takes_arguments());
        }
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
