use std::fs;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Terminal, TerminalOptions, Viewport};
use uscope::{BreakpointSpec, Debugger, Error, StopReason};

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Native executable to debug.
    #[arg(value_name = "EXECUTABLE")]
    executable: PathBuf,

    /// Execute commands from a file. May be repeated.
    #[arg(short = 'x', long = "command", value_name = "FILE")]
    command_files: Vec<PathBuf>,

    /// Execute one command. May be repeated.
    #[arg(short = 'e', long = "eval", value_name = "COMMAND")]
    commands: Vec<String>,

    /// Print plain-text results without opening the terminal UI.
    #[arg(long)]
    batch: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let mut debugger = Debugger::new(&args.executable)?;
    let result = run(&debugger, &args);
    let shutdown = debugger.shutdown();
    result?;
    shutdown?;
    Ok(())
}

fn run(debugger: &Debugger, args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let mut output = vec![format!("debugging {}", debugger.executable().display())];
    for path in &args.command_files {
        let contents = fs::read_to_string(path)?;
        if !run_lines(
            debugger,
            contents.lines(),
            &path.display().to_string(),
            args.batch,
            &mut output,
        )? {
            return Ok(());
        }
    }
    for (index, command) in args.commands.iter().enumerate() {
        if !run_line(
            debugger,
            command,
            &format!("--eval #{}", index + 1),
            args.batch,
            &mut output,
        )? {
            return Ok(());
        }
    }
    if args.batch {
        if args.command_files.is_empty() && args.commands.is_empty() {
            let mut stdin = io::stdin().lock();
            let mut line = String::new();
            let mut number = 0;
            loop {
                line.clear();
                if stdin.read_line(&mut line)? == 0 {
                    break;
                }
                number += 1;
                if !run_line(
                    debugger,
                    &line,
                    &format!("stdin:{number}"),
                    true,
                    &mut output,
                )? {
                    break;
                }
            }
            drop(stdin);
        }
        Ok(())
    } else {
        repl(debugger, output)
    }
}

fn run_lines<'a>(
    debugger: &Debugger,
    lines: impl Iterator<Item = &'a str>,
    source: &str,
    batch: bool,
    output: &mut Vec<String>,
) -> Result<bool, Box<dyn std::error::Error>> {
    for (index, line) in lines.enumerate() {
        if !run_line(
            debugger,
            line,
            &format!("{source}:{}", index + 1),
            batch,
            output,
        )? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn run_line(
    debugger: &Debugger,
    line: &str,
    source: &str,
    batch: bool,
    output: &mut Vec<String>,
) -> Result<bool, Box<dyn std::error::Error>> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(true);
    }
    match execute(debugger, line).map_err(|error| io::Error::other(format!("{source}: {error}")))? {
        Control::Continue(message) => {
            if !message.is_empty() {
                if batch {
                    println!("{message}");
                    io::stdout().flush()?;
                } else {
                    output.push(format!("> {line}"));
                    output.push(message);
                }
            }
            Ok(true)
        }
        Control::Quit => Ok(false),
    }
}

fn repl(debugger: &Debugger, output: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let backend = CrosstermBackend::new(io::stdout());
    let options = TerminalOptions {
        viewport: Viewport::Inline(12),
    };
    let mut terminal = Terminal::with_options(backend, options)?;
    let result = run_repl(&mut terminal, debugger, output);
    disable_raw_mode()?;
    terminal.show_cursor()?;
    result
}

fn run_repl(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    debugger: &Debugger,
    mut output: Vec<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut input = String::new();
    loop {
        terminal.draw(|frame| {
            let [history, prompt] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).areas(frame.area());
            let visible = history.height.saturating_sub(2) as usize;
            let start = output.len().saturating_sub(visible);
            let lines: Vec<Line<'_>> = output[start..]
                .iter()
                .map(String::as_str)
                .map(Line::from)
                .collect();
            frame.render_widget(
                Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("uscope")),
                history,
            );
            frame.render_widget(
                Paragraph::new(format!("> {input}")).block(Block::default().borders(Borders::ALL)),
                prompt,
            );
            frame.set_cursor_position((prompt_cursor_x(prompt, input.len()), prompt.y + 1));
        })?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
            KeyCode::Char(character) => input.push(character),
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Enter => {
                let command = std::mem::take(&mut input);
                match run_line(debugger, &command, "repl", false, &mut output) {
                    Ok(true) => {}
                    Ok(false) => return Ok(()),
                    Err(error) => output.push(format!("error: {error}")),
                }
            }
            _ => {}
        }
    }
}

fn prompt_cursor_x(prompt: Rect, input_len: usize) -> u16 {
    let input_width = u16::try_from(input_len).unwrap_or(u16::MAX);
    prompt
        .x
        .saturating_add(3)
        .saturating_add(input_width)
        .min(prompt.right().saturating_sub(2))
}

enum Control {
    Continue(String),
    Quit,
}

fn execute(debugger: &Debugger, line: &str) -> uscope::Result<Control> {
    let mut words = line.split_whitespace();
    let command = words.next().unwrap_or("");
    match command {
        "break" | "b" => {
            let argument = one_argument(&mut words, "break <function|address>")?;
            let spec = parse_address(argument).map_or_else(
                |_| BreakpointSpec::Function(argument.to_owned()),
                BreakpointSpec::Address,
            );
            let address = debugger.add_breakpoint(spec)?;
            Ok(Control::Continue(format!(
                "breakpoint set at link/runtime address {address:#x}"
            )))
        }
        "run" | "r" => Ok(Control::Continue(format_stop(debugger.run()?))),
        "continue" | "c" => Ok(Control::Continue(format_stop(debugger.resume()?))),
        "x" => {
            let address = parse_address(one_argument(&mut words, "x <runtime-address>")?)?;
            Ok(Control::Continue(format!(
                "{address:#018x}: {:#018x}",
                debugger.read_word(address)?
            )))
        }
        "address" => {
            let name = one_argument(&mut words, "address <symbol>")?;
            Ok(Control::Continue(format!(
                "{name}: {:#x}",
                debugger.runtime_address(name)?
            )))
        }
        "quit" | "q" => Ok(Control::Quit),
        "" => Ok(Control::Continue(String::new())),
        other => Err(Error::InvalidCommand(other.to_owned())),
    }
}

fn one_argument<'a>(
    words: &mut impl Iterator<Item = &'a str>,
    usage: &str,
) -> uscope::Result<&'a str> {
    let argument = words
        .next()
        .ok_or_else(|| Error::InvalidCommand(usage.to_owned()))?;
    if words.next().is_some() {
        return Err(Error::InvalidCommand(usage.to_owned()));
    }
    Ok(argument)
}

fn parse_address(value: &str) -> uscope::Result<u64> {
    let value = value.strip_prefix("0x").unwrap_or(value);
    u64::from_str_radix(value, 16)
        .map_err(|_| Error::InvalidCommand(format!("invalid hexadecimal address: {value}")))
}

fn format_stop(reason: StopReason) -> String {
    match reason {
        StopReason::Breakpoint { address } => format!("stopped at breakpoint {address:#x}"),
        StopReason::Signal(signal) => format!("stopped by {signal}"),
        StopReason::Exited(code) => format!("inferior exited with status {code}"),
        StopReason::Signaled(signal) => format!("inferior terminated by {signal}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_cursor_follows_input_and_stays_inside_border() {
        let prompt = Rect::new(10, 0, 20, 3);

        assert_eq!(prompt_cursor_x(prompt, 0), 13);
        assert_eq!(prompt_cursor_x(prompt, 5), 18);
        assert_eq!(prompt_cursor_x(prompt, usize::MAX), 28);
    }
}
