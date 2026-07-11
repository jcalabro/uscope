use std::io;
use std::path::PathBuf;

use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Terminal, TerminalOptions, Viewport};
use uscope::{BreakpointSpec, Debugger, Error, StopReason};

#[derive(Parser)]
#[command(version, about)]
struct Args {
    #[arg(value_name = "EXECUTABLE")]
    executable: PathBuf,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let mut debugger = Debugger::new(args.executable)?;
    let result = repl(&debugger);
    let shutdown = debugger.shutdown();
    result?;
    shutdown?;
    Ok(())
}

fn repl(debugger: &Debugger) -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let backend = CrosstermBackend::new(io::stdout());
    let options = TerminalOptions {
        viewport: Viewport::Inline(12),
    };
    let mut terminal = Terminal::with_options(backend, options)?;
    let result = run_repl(&mut terminal, debugger);
    disable_raw_mode()?;
    terminal.show_cursor()?;
    result
}

fn run_repl(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    debugger: &Debugger,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut input = String::new();
    let mut output = vec![format!("debugging {}", debugger.executable().display())];
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
            frame.set_cursor_position((prompt.x + 2 + input.len() as u16, prompt.y + 1));
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
                output.push(format!("> {command}"));
                match execute(debugger, &command) {
                    Ok(Control::Continue(message)) => output.push(message),
                    Ok(Control::Quit) => return Ok(()),
                    Err(error) => output.push(format!("error: {error}")),
                }
            }
            _ => {}
        }
    }
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
            let spec = match parse_address(argument) {
                Ok(address) => BreakpointSpec::Address(address),
                Err(_) => BreakpointSpec::Function(argument.to_owned()),
            };
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
